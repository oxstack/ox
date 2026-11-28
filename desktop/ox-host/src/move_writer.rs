//! Durable evidence for the exact writer fenced by a move. A host restart
//! closes that helper's lifetime lease. Ownership still waits for its group to exit.
use serde::{Deserialize, Serialize};
use std::io;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct WriterGroup {
    id: u32,
    birth: Option<String>,
}

impl WriterGroup {
    pub(crate) fn record(id: u32) -> io::Result<Self> {
        let pid =
            process_id(id).ok_or_else(|| io::Error::other("Invalid source process group."))?;
        Ok(Self {
            id,
            birth: group_birth(pid),
        })
    }

    pub(crate) fn retired(&self) -> bool {
        let Some(pid) = process_id(self.id) else {
            return false;
        };
        // Signal zero probes membership without signaling any process. The
        // direct helper can exit before its stock runtime, so probe the group.
        if unsafe { libc::kill(-pid, 0) } != 0
            && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            return true;
        }
        // The OS cannot recycle a group ID while any of its members remain.
        // A different leader's birth proves the original group has retired.
        self.birth
            .as_ref()
            .zip(group_birth(pid))
            .is_some_and(|(before, now)| *before != now)
            || group_has_only_exited_members(pid)
    }
}

fn process_id(id: u32) -> Option<libc::pid_t> {
    libc::pid_t::try_from(id).ok().filter(|pid| *pid > 1)
}

#[cfg(target_os = "linux")]
fn group_birth(pid: libc::pid_t) -> Option<String> {
    let stat = process_stat(pid).ok()?;
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    (stat.group == pid).then(|| format!("{}:{}", boot.trim(), stat.ticks))
}

#[cfg(target_os = "linux")]
struct Stat {
    state: String,
    group: libc::pid_t,
    ticks: u64,
}

#[cfg(target_os = "linux")]
fn process_stat(pid: libc::pid_t) -> io::Result<Stat> {
    let raw = std::fs::read(format!("/proc/{pid}/stat"))?;
    let parse = || {
        // Linux permits arbitrary bytes in the process name. The numeric
        // fields follow its last closing parenthesis.
        let close = raw.iter().rposition(|byte| *byte == b')')?;
        let fields = std::str::from_utf8(raw.get(close + 1..)?).ok()?;
        let mut fields = fields.split_whitespace();
        let state = fields.next()?.to_owned();
        let group = fields.nth(1)?.parse().ok()?;
        let ticks = fields.nth(16)?.parse().ok()?;
        Some(Stat {
            state,
            group,
            ticks,
        })
    };
    parse().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Invalid process status."))
}

#[cfg(target_os = "linux")]
fn group_has_only_exited_members(group: libc::pid_t) -> bool {
    if process_stat(group)
        .is_ok_and(|stat| stat.group == group && !matches!(stat.state.as_str(), "Z" | "X"))
    {
        return false;
    }
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    let mut exited = false;
    for entry in entries {
        let Ok(entry) = entry else { return false };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        else {
            continue;
        };
        match process_stat(pid) {
            Ok(stat) if stat.group != group => (),
            Ok(stat) if matches!(stat.state.as_str(), "Z" | "X") => exited = true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            _ => return false,
        }
    }
    exited
}

#[cfg(target_os = "macos")]
fn group_has_only_exited_members(_group: libc::pid_t) -> bool {
    // macOS reaps the orphaned group via launchd. The group probe above waits
    // for it to disappear, or recognizes a reused group by its leader's birth.
    false
}

#[cfg(target_os = "macos")]
fn group_birth(pid: libc::pid_t) -> Option<String> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let bytes = std::mem::size_of_val(&info) as libc::c_int;
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::from_mut(&mut info).cast(),
            bytes,
        )
    };
    (read == bytes && info.pbi_pid == pid as u32 && info.pbi_pgid == pid as u32)
        .then(|| format!("{}:{}", info.pbi_start_tvsec, info.pbi_start_tvusec))
}
