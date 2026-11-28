mod control;
mod doctor;
mod durable;
mod engine;
#[cfg(test)]
#[path = "../../../tests/common/executable.rs"]
mod executable;
mod handover;
mod identity;
mod lease;
mod move_archive;
mod move_bundle;
mod move_ledger;
mod move_target;
mod move_writer;
mod pairing;
mod policy;
mod projects;
mod receipts;
mod replay;
mod resources;
mod runtime;
mod transport;
mod updates;
mod worker_notices;
#[path = "../../../src/worker_placement.rs"]
mod worker_placement;
mod workers;
mod yolo;

use engine::{Engine, Message};
use ox_host_service::{Paths, SocketGuard};
use std::{
    env,
    io::{self, Read},
    os::unix::{io::AsRawFd, net::UnixStream},
    path::PathBuf,
    sync::{
        atomic::{AtomicI32, Ordering},
        mpsc,
    },
    thread,
};

static SIGNAL_FD: AtomicI32 = AtomicI32::new(-1);
extern "C" fn stop_signal(signal: libc::c_int) {
    // write is async-signal-safe. The reader wakes the event-driven actor.
    unsafe {
        libc::write(
            SIGNAL_FD.load(Ordering::Relaxed),
            [signal as u8].as_ptr().cast(),
            1,
        );
    }
}
fn report_staged_update() {
    use ox_host_service::managed_install::{active_binary, package_store};
    let Some(store) = env::current_exe().ok().and_then(|p| package_store(&p)) else {
        return;
    };
    if !store.join("pending.json").is_file() {
        return;
    }
    let reply = std::process::Command::new(active_binary(&store, "ox"))
        .args(["update", "--status", "--json"])
        .output();
    if let Ok(output) = reply
        && output.status.success()
        && let Ok(status) = serde_json::from_slice::<serde_json::Value>(&output.stdout)
        && let Some(version) = status["staged"].as_str()
    {
        println!("OX {version} is downloaded. Run ox update to install it.");
    }
}

fn run(paths: Paths) -> io::Result<()> {
    ox_host_service::configure_allocator();
    let guard = SocketGuard::bind(&paths)?;
    ox_host_service::errlog::init(&paths.state_dir);
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ox_host_service::errlog::record(&format!("panic: {info}"));
        default_hook(info);
    }));
    let (tx, rx) = mpsc::sync_channel(512);
    let lease = lease::Endpoint::bind(&paths.socket)?;
    // Only the host's own runtime children receive this endpoint. Set it before starting threads.
    unsafe {
        env::set_var("OX_HOST_LIFETIME_SOCKET", &lease.path);
    }
    let mut engine = Engine::open(
        runtime::RuntimeConfig::discover()?,
        &paths.state_dir,
        tx.clone(),
    )?;
    let (mut signal_input, signal_output) = UnixStream::pair()?;
    signal_output.set_nonblocking(true)?;
    SIGNAL_FD.store(signal_output.as_raw_fd(), Ordering::Relaxed);
    unsafe {
        let mut act: libc::sigaction = std::mem::zeroed();
        act.sa_sigaction = stop_signal as *const () as usize;
        libc::sigemptyset(&mut act.sa_mask);
        // SIGUSR1 asks for an upgrade drain (see `ox host restart`).
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGUSR1] {
            if libc::sigaction(signal, &act, std::ptr::null_mut()) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    let signals = tx.clone();
    let drain_bound = paths.state_dir.clone();
    thread::spawn(move || {
        let mut byte = [0];
        while signal_input.read_exact(&mut byte).is_ok() {
            if i32::from(byte[0]) == libc::SIGUSR1 {
                let bound = control::drain_bound(&drain_bound);
                let _ = signals.send(Message::Drain(bound));
            } else {
                let _ = signals.send(Message::Stop);
                break;
            }
        }
    });
    updates::start(paths.clone(), tx.clone())?;
    lease.listen(tx.clone())?;
    let jobs = tx.clone();
    let listener = guard.listener.try_clone()?;
    thread::spawn(move || {
        for (id, next) in listener.incoming().enumerate() {
            match next {
                Ok(stream) => {
                    let _ = transport::attach(stream, id as u64, tx.clone());
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });
    let web_paths = paths.clone();
    let moves = engine.move_receiver();
    thread::spawn(move || {
        let dispatch = std::sync::Arc::new(move |job| {
            let (reply, response) = mpsc::sync_channel(1);
            jobs.send(Message::LinkedJob(
                Box::new(job),
                engine::Replier::Channel(reply),
            ))
            .map_err(|_| "The target host stopped.".to_owned())?;
            response
                .recv()
                .map_err(|_| "The target worker engine stopped.".to_owned())?
        });
        if let Err(e) = ox_host_web::run_with_receivers(
            web_paths.state_dir,
            web_paths.socket,
            Some(dispatch),
            Some(moves),
        ) {
            ox_host_service::errlog::record(&format!("OX remote service: {e}"));
        }
    });
    let result = engine.serve(rx);
    SIGNAL_FD.store(-1, Ordering::Relaxed);
    if let Err(e) = &result {
        ox_host_service::errlog::record(&format!("stopped: {e}"));
    }
    result?;
    if let Some(binary) = engine.replacement.take() {
        use std::os::unix::process::CommandExt;
        drop(engine);
        drop(lease);
        drop(guard);
        return Err(std::process::Command::new(binary)
            .arg("run")
            .arg("--state-dir")
            .arg(paths.state_dir)
            .arg("--socket")
            .arg(paths.socket)
            .exec());
    }
    Ok(())
}
const USAGE: &str = "Usage: ox-host COMMAND [--socket PATH] [--state-dir PATH] [ARGS]
  run                       serve sessions (what the login service runs)
  install                   start the host at login (launchd or systemd user service)
  status                    show whether the host and its login service run
  stop                      stop the host now; running turns are interrupted
  restart [--timeout SECS]  finish running turns (default bound 600s), then start the installed host
  pair [--terminal] [--port HTTPS_PORT] [--listen-port LOCAL_PORT]
                            print a private Serve invitation, optionally granting a terminal
  pair --worker-host [--port HTTPS_PORT] [--listen-port LOCAL_PORT]
                            invite another OX host to launch workers here
  update [setting]          install when idle or set automatic, download, manual, stable or dev
  name [NAME]               show or set the name shown to clients
  policy [set POLICY]       show or set the starting mode for this host
  labels [set LABEL…]       show or replace placement labels
  devices                   list paired devices and their IDs
  terminal <device> on|off  change a paired device’s terminal grant
  memory-sync [<device> on|off]
                            sync non-local memories with workers, or change a worker link’s memory-sync grant
  revoke <device-id>        disconnect a paired device
  pending                   list devices waiting to pair through a window
  approve <request-id>      accept one of them, registering the device
  deny <request-id>         refuse it
  doctor [--errors N]       check the host, Tailscale Serve, paired devices and stock clients
  uninstall [--purge]       remove the login service; --purge also deletes OX host state
                            (session ledger, pairings, push keys). Codex and Claude Code data is never touched.";
fn entry() -> io::Result<()> {
    let mut args = env::args_os().skip(1);
    let op = args.next().unwrap_or_else(|| "run".into());
    if op == "--version" || op == "-V" {
        println!(
            "ox-host {}",
            option_env!("OX_BUILD_VERSION")
                .filter(|s| !s.is_empty())
                .unwrap_or(env!("CARGO_PKG_VERSION"))
        );
        return Ok(());
    }
    if matches!(op.to_str(), Some("-h" | "--help" | "help")) {
        println!("{USAGE}");
        return Ok(());
    }
    let mut paths = Paths::discover()?;
    let mut args = args.peekable();
    while let Some(flag) = args.next_if(|a| a == "--socket" || a == "--state-dir") {
        let dest = if flag == "--socket" {
            &mut paths.socket
        } else if flag == "--state-dir" {
            &mut paths.state_dir
        } else {
            return Err(io::Error::other(
                "Expected --socket PATH or --state-dir PATH",
            ));
        };
        *dest = PathBuf::from(
            args.next()
                .ok_or_else(|| io::Error::other("Missing host path"))?,
        );
    }
    let rest: Vec<String> = args
        .by_ref()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    if matches!(op.to_str(), Some("status" | "doctor")) {
        report_staged_update();
    }
    match (op.to_str(), rest.as_slice()) {
        (Some("run"), []) => run(paths),
        (Some("install"), []) => ox_host_service::manage("install", &paths),
        (Some("status"), []) => {
            ox_host_service::manage("status", &paths)?;
            if ox_host_service::is_running(&paths).unwrap_or(false)
                && let Ok(reply) = ox_host_web::admin::request(
                    &paths.state_dir,
                    &ox_host_web::admin::Request::Status,
                )
            {
                match (reply.enabled, reply.origin) {
                    (true, Some(origin)) if reply.port > 0 => {
                        println!("Remote access: {origin} → 127.0.0.1:{}", reply.port);
                    }
                    (true, Some(origin)) => println!("Remote access: {origin}"),
                    _ => println!("Remote access: off"),
                }
            }
            Ok(())
        }
        (Some("install" | "status" | "stop" | "devices"), [flag])
            if matches!(flag.as_str(), "-h" | "--help") =>
        {
            println!("{USAGE}");
            Ok(())
        }
        (Some("uninstall"), []) => ox_host_service::manage("uninstall", &paths),
        (Some("uninstall"), [purge]) if purge == "--purge" => control::purge(&paths),
        (Some("uninstall"), [flag]) if matches!(flag.as_str(), "-h" | "--help") => {
            println!("Usage: ox host uninstall [--purge]");
            Ok(())
        }
        (Some("stop"), []) => control::stop(&paths),
        (Some("restart-if-updated"), []) => control::restart_if_updated(&paths),
        (Some("restart"), [flag]) if matches!(flag.as_str(), "-h" | "--help") => {
            println!("Usage: ox host restart [--timeout SECS]");
            Ok(())
        }
        (Some("restart"), rest) => control::restart(&paths, control::timeout(rest)?),
        (Some(kind @ ("name" | "labels")), rest) => pairing::profile(&paths, kind, rest),
        (Some("project"), rest) => projects::cli(&paths.state_dir, rest),
        (Some("policy"), rest) => pairing::mode_policy(&paths, rest),
        (Some("update"), rest) => updates::cli(&paths, rest),
        (Some("pair"), rest) => pairing::pair_cli(&paths, rest).map_err(io::Error::other),
        (Some("devices"), []) => pairing::devices(&paths).map_err(io::Error::other),
        (Some("terminal"), [flag]) if matches!(flag.as_str(), "-h" | "--help") => {
            println!("Usage: ox host terminal <device> on|off");
            Ok(())
        }
        (Some("terminal"), [id, setting]) if matches!(setting.as_str(), "on" | "off") => {
            pairing::terminal(&paths, id, setting == "on").map_err(io::Error::other)
        }
        (Some("revoke"), [flag]) if matches!(flag.as_str(), "-h" | "--help") => {
            println!("Usage: ox host revoke <device-id>");
            Ok(())
        }
        (Some("revoke"), [id]) => pairing::revoke(&paths, id).map_err(io::Error::other),
        (Some("pending"), []) => pairing::pending(&paths).map_err(io::Error::other),
        (Some("approve"), [flag]) if matches!(flag.as_str(), "-h" | "--help") => {
            println!("Usage: ox host approve <request-id>");
            Ok(())
        }
        (Some("approve"), [id]) => pairing::approve(&paths, id).map_err(io::Error::other),
        (Some("deny"), [id]) => pairing::deny(&paths, id).map_err(io::Error::other),
        (Some("doctor"), rest) => doctor::run(&paths, rest),
        (Some("memory-sync"), []) => {
            let host_id = identity::load(&paths.state_dir)?.to_string();
            let results = ox_host_web::memory_sync::sync_all_peers(&paths.state_dir, &host_id)?;
            if results.is_empty() {
                println!("No paired worker hosts configured.");
            } else {
                let mut failures = 0usize;
                for (name, res) in results {
                    match res {
                        Ok(stats) => println!(
                            "{name}: synced (+{} inserted, ~{} updated, {} ignored)",
                            stats.inserted, stats.updated, stats.ignored
                        ),
                        Err(err) => {
                            failures += 1;
                            eprintln!("{name}: sync failed: {err}");
                        }
                    }
                }
                if failures > 0 {
                    return Err(io::Error::other(format!(
                        "{failures} peer(s) failed to synchronize."
                    )));
                }
            }
            Ok(())
        }
        (Some("memory-sync"), [id, setting]) if matches!(setting.as_str(), "on" | "off") => {
            pairing::memory_sync(&paths, id, setting == "on").map_err(io::Error::other)
        }
        _ => Err(io::Error::other(USAGE)),
    }
}
fn main() -> std::process::ExitCode {
    let _package = ox_host_service::managed_install::hold_generation();
    match entry() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ox-host: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
#[test]
#[ignore = "disposable two-host integration using its own TLS trust store"]
fn linked_parent_fixture() {
    ox_host_web::client::enable_fixture_transport();
    run(Paths {
        socket: env::var_os("OX_HOST_SOCKET").unwrap().into(),
        state_dir: env::var_os("OX_HOST_STATE_DIR").unwrap().into(),
    })
    .unwrap();
}
