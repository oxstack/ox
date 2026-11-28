//! An operator-owned terminal process and bounded in-memory VT screen.

use std::{
    collections::VecDeque,
    fs::File,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::{net::UnixStream, process::CommandExt},
    },
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
};

pub type Size = (u16, u16);
pub type Check = Box<dyn FnOnce() -> Result<(), String> + Send>;
pub type Outcome = Result<Option<i32>, String>;
pub type LineMatcher = Box<dyn Fn(&str) -> bool + Send>;
pub struct Screen {
    pub parser: vt100::Parser,
    pub outcome: Option<Outcome>,
    /// Reported only to an operator viewing this PTY. Unknown modes are treated as private.
    pub echo: Option<bool>,
    line_capture: Option<LineCapture>,
    captured_line: Option<String>,
}

struct LineCapture {
    matches: LineMatcher,
    pending: Vec<u8>,
    overflow: bool,
}

impl LineCapture {
    fn process(&mut self, bytes: &[u8]) -> Option<String> {
        const MAX_LINE_BYTES: usize = 8192;
        for chunk in bytes.split_inclusive(|&b| b == b'\n') {
            let line = chunk.strip_suffix(b"\n");
            let data = line.unwrap_or(chunk);
            if self.pending.len() + data.len() > MAX_LINE_BYTES {
                self.overflow = true;
            }
            if !self.overflow {
                self.pending.extend(data);
            }
            if line.is_some() {
                if !self.overflow
                    && let Ok(line) = std::str::from_utf8(&self.pending)
                    && (self.matches)(line.trim())
                {
                    return Some(line.trim().to_owned());
                }
                self.pending.clear();
                self.overflow = false;
            }
        }
        None
    }
}

impl Screen {
    /// Read the first complete matching line as the process wrote it, before terminal wrapping.
    pub fn take_captured_line(&mut self) -> Option<String> {
        self.captured_line.take()
    }

    fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
        if let Some(capture) = &mut self.line_capture
            && let Some(line) = capture.process(bytes)
        {
            self.captured_line = Some(line);
            self.line_capture = None;
        }
    }
}

struct Display {
    size: Size,
    observe_echo: bool,
}

pub struct Terminal {
    input: flume::Sender<Vec<u8>>,
    wake: UnixStream,
    display: Arc<Mutex<Display>>,
}

impl Terminal {
    pub fn start(
        project: PathBuf,
        command: TerminalCommand,
        check: Check,
        size: Size,
    ) -> io::Result<(Self, Arc<Mutex<Screen>>, flume::Receiver<()>)> {
        Self::start_with_line_capture(project, command, check, size, None)
    }

    pub fn start_with_line_capture(
        project: PathBuf,
        command: TerminalCommand,
        check: Check,
        size: Size,
        matcher: Option<LineMatcher>,
    ) -> io::Result<(Self, Arc<Mutex<Screen>>, flume::Receiver<()>)> {
        let (wake, mut listener) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        listener.set_nonblocking(true)?;
        let (input, events) = flume::bounded(64);
        let (changed, updates) = flume::bounded(1);
        let screen = Arc::new(Mutex::new(Screen {
            parser: vt100::Parser::new(size.0, size.1, 300),
            outcome: None,
            echo: None,
            line_capture: matcher.map(|matches| LineCapture {
                matches,
                pending: Vec::new(),
                overflow: false,
            }),
            captured_line: None,
        }));
        let terminal = Self {
            input,
            wake,
            display: Arc::new(Mutex::new(Display {
                size,
                observe_echo: false,
            })),
        };
        let (worker_screen, desired) = (Arc::clone(&screen), Arc::clone(&terminal.display));
        std::thread::Builder::new()
            .name("ox-local-pty".into())
            .spawn(move || {
                let result = check().and_then(|_| {
                    // A review may have closed while the preflight was running.
                    if events.is_disconnected() {
                        return Ok(None);
                    }
                    let (mut master, mut child) =
                        spawn(project, command, size).map_err(|e| e.to_string())?;
                    // The waiter signals the poll loop on exit without reaping the leader. Its
                    // PID remains reserved until we have closed the entire PTY session.
                    let result = supervise(
                        &mut master,
                        &mut child,
                        &mut listener,
                        &events,
                        &desired,
                        &worker_screen,
                        &changed,
                    );
                    result.map_err(|e| e.to_string())
                });
                worker_screen.lock().unwrap().outcome = Some(result);
                let _ = changed.try_send(());
            })?;
        Ok((terminal, screen, updates))
    }

    pub fn send(&self, data: Vec<u8>) -> bool {
        if data.len() > 32 * 1024 || self.input.try_send(data).is_err() {
            return false;
        }
        self.notify();
        true
    }
    pub fn resize(&self, size: Size) {
        let mut desired = self.display.lock().unwrap();
        if desired.size != size {
            desired.size = size;
            self.notify();
        }
    }
    pub fn observe_echo(&self) {
        self.display.lock().unwrap().observe_echo = true;
        self.notify();
    }
    fn notify(&self) {
        let _ = (&self.wake).write(&[1]);
    }
}

fn syscall(result: libc::c_int) -> io::Result<libc::c_int> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

fn open_pair() -> io::Result<(File, File)> {
    // Open with close-on-exec from creation; other desktop threads can spawn children concurrently.
    let flags = libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC;
    unsafe {
        let master = File::from_raw_fd(syscall(libc::posix_openpt(flags))?);
        let fd = master.as_raw_fd();
        syscall(libc::grantpt(fd))?;
        syscall(libc::unlockpt(fd))?;
        #[cfg(target_os = "linux")]
        let slave = syscall(libc::ioctl(fd, libc::TIOCGPTPEER, flags))?;
        #[cfg(target_os = "macos")]
        let slave = {
            let mut path = [0 as libc::c_char; 128];
            syscall(libc::ioctl(
                fd,
                libc::TIOCPTYGNAME.into(),
                path.as_mut_ptr(),
            ))?;
            syscall(libc::open(path.as_ptr(), flags))?
        };
        Ok((master, File::from_raw_fd(slave)))
    }
}

fn winsize(fd: RawFd, (rows, cols): Size) -> io::Result<()> {
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        syscall(libc::ioctl(fd, libc::TIOCSWINSZ, &size))?;
    }
    Ok(())
}

fn spawn(project: PathBuf, command: TerminalCommand, size: Size) -> io::Result<(File, Child)> {
    let (master, slave) = open_pair()?;
    let fd = master.as_raw_fd();
    winsize(fd, size)?;
    unsafe {
        syscall(libc::fcntl(
            fd,
            libc::F_SETFL,
            syscall(libc::fcntl(fd, libc::F_GETFL))? | libc::O_NONBLOCK,
        ))?;
    }
    let mut process = Command::new(command.program);
    process
        .args(command.args)
        .current_dir(project)
        .env("TERM", "xterm-256color")
        .env("HISTFILE", "/dev/null")
        .env("LESSHISTFILE", "-")
        .env("PATH", child_path())
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    // Only async-signal-safe syscalls between fork and exec. stdin already refers to the slave.
    unsafe {
        process.pre_exec(|| {
            syscall(libc::setsid())?;
            syscall(libc::ioctl(0, libc::TIOCSCTTY as _, 0))?;
            Ok(())
        });
    }
    Ok((master, process.spawn()?))
}

fn supervise(
    master: &mut File,
    child: &mut Child,
    wake: &mut UnixStream,
    input: &flume::Receiver<Vec<u8>>,
    desired: &Mutex<Display>,
    screen: &Mutex<Screen>,
    changed: &flume::Sender<()>,
) -> io::Result<Option<i32>> {
    let leader = child.id() as libc::pid_t;
    let result = std::thread::scope(|scope| -> io::Result<bool> {
        let (exit, watched) = UnixStream::pair()?;
        let waiter = std::thread::Builder::new()
            .name("ox-local-pty-wait".into())
            .spawn_scoped(scope, move || {
                // Closing this socket wakes poll even when a background job retains the slave.
                let result = loop {
                    let mut info = std::mem::MaybeUninit::zeroed();
                    let result = unsafe {
                        libc::waitid(
                            libc::P_PID,
                            leader as _,
                            info.as_mut_ptr(),
                            libc::WEXITED | libc::WNOWAIT,
                        )
                    };
                    if result >= 0 {
                        break Ok(());
                    }
                    let e = io::Error::last_os_error();
                    if e.kind() != io::ErrorKind::Interrupted {
                        break Err(e);
                    }
                };
                drop(exit);
                result
            })?;
        let result = pump(
            master,
            wake,
            watched.as_raw_fd(),
            input,
            desired,
            screen,
            changed,
        );
        terminate(leader, master.as_raw_fd());
        waiter
            .join()
            .map_err(|_| io::Error::other("Terminal wait failed"))??;
        result
    });
    // Also clean up if creating the wait channel itself failed.
    if result.is_err() {
        terminate(leader, master.as_raw_fd());
    }
    let status = child.wait()?;
    Ok(if result? { status.code() } else { None })
}

#[cfg(target_os = "linux")]
fn session_candidates() -> Vec<libc::pid_t> {
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
        .collect()
}
#[cfg(target_os = "macos")]
fn session_candidates() -> Vec<libc::pid_t> {
    // libproc returns a count of PIDs, not a byte length. Only live members of this
    // specific session will receive a signal.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return Vec::new();
    }
    let mut pids = vec![0; count as usize + 128];
    let n = unsafe {
        libc::proc_listallpids(
            pids.as_mut_ptr().cast(),
            (pids.len() * std::mem::size_of::<libc::pid_t>()) as _,
        )
    };
    pids.truncate(n.max(0) as usize);
    pids
}

fn terminate(leader: libc::pid_t, master: RawFd) {
    // Keep the session leader unreaped until its process groups have been signalled.

    unsafe {
        let foreground = libc::tcgetpgrp(master);
        if foreground > 0 && foreground != leader {
            libc::kill(-foreground, libc::SIGHUP);
            libc::kill(-foreground, libc::SIGKILL);
        }
        libc::kill(-leader, libc::SIGHUP);
        libc::kill(-leader, libc::SIGKILL);
    }
    // A job-control shell puts background pipelines in additional process groups.
    for pid in session_candidates() {
        if pid != leader && pid > 0 && unsafe { libc::getsid(pid) } == leader {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

fn pump(
    master: &mut File,
    wake: &mut UnixStream,
    exited: RawFd,
    input: &flume::Receiver<Vec<u8>>,
    desired: &Mutex<Display>,
    screen: &Mutex<Screen>,
    changed: &flume::Sender<()>,
) -> io::Result<bool> {
    let mut pending = VecDeque::<u8>::new();
    let mut buffer = [0; 16384];
    let mut size = screen.lock().unwrap().parser.screen().size();
    let mut slave_open = true;
    loop {
        if input.is_disconnected() {
            return Ok(false);
        }
        if pending.is_empty()
            && let Ok(bytes) = input.try_recv()
        {
            pending.extend(bytes);
        }
        let (new_size, observe_echo) = {
            let display = desired.lock().unwrap();
            (display.size, display.observe_echo && slave_open)
        };
        if slave_open && new_size != size {
            winsize(master.as_raw_fd(), new_size)?;
            screen
                .lock()
                .unwrap()
                .parser
                .screen_mut()
                .set_size(new_size.0, new_size.1);
            size = new_size;
            let _ = changed.try_send(());
        }
        let mut fds = [
            libc::pollfd {
                fd: if slave_open { master.as_raw_fd() } else { -1 },
                events: libc::POLLIN | if pending.is_empty() { 0 } else { libc::POLLOUT },
                revents: 0,
            },
            libc::pollfd {
                fd: wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: exited,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // A password reader can switch termios after writing its prompt, or without any
        // output at all. Only a live phone terminal needs this bounded idle mode check.
        let timeout = if observe_echo { 200 } else { -1 };
        if unsafe { libc::poll(fds.as_mut_ptr(), 3, timeout) } < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if observe_echo {
            let mut mode = std::mem::MaybeUninit::<libc::termios>::uninit();
            let echo = (unsafe { libc::tcgetattr(master.as_raw_fd(), mode.as_mut_ptr()) } == 0)
                .then(|| unsafe { mode.assume_init() }.c_lflag & libc::ECHO != 0);
            let mut view = screen.lock().unwrap();
            if view.echo != echo {
                view.echo = echo;
                let _ = changed.try_send(());
            }
        }
        if fds[1].revents != 0 {
            let _ = wake.read(&mut buffer);
            if input.is_disconnected() {
                return Ok(false);
            }
        }
        if fds[0].revents & libc::POLLOUT != 0 && !pending.is_empty() {
            match master.write(pending.as_slices().0) {
                Ok(n) => {
                    pending.drain(..n);
                }
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        if fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            match master.read(&mut buffer) {
                Ok(n) if n > 0 => {
                    screen.lock().unwrap().process(&buffer[..n]);
                    let _ = changed.try_send(());
                    if fds[2].revents == 0 {
                        continue;
                    }
                    // Drain the child's last writes even when exit and output are reported together.
                    // Cap the tail in case a surviving background writer is still flooding the PTY.
                    for _ in 0..4 {
                        match master.read(&mut buffer) {
                            Ok(n) if n > 0 => screen.lock().unwrap().process(&buffer[..n]),
                            _ => break,
                        }
                    }
                }
                Ok(0) => slave_open = false,
                Err(e) if e.raw_os_error() == Some(libc::EIO) => slave_open = false,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(e),
                _ => {}
            }
        }
        if fds[2].revents != 0 {
            return Ok(true);
        }
    }
}

use std::{
    env,
    ffi::{OsStr, OsString},
};
#[derive(Clone)]
pub struct TerminalCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

impl TerminalCommand {
    pub fn shell() -> Self {
        let program = std::env::var_os("SHELL")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| PathBuf::from("/bin/sh"));
        Self {
            program,
            args: vec!["-i".into()],
        }
    }

    /// The phone edits whole lines locally. Bash readline and Zsh ZLE otherwise keep ECHO
    /// disabled at ordinary prompts, making them indistinguishable from password input.
    pub fn line_input_shell() -> Self {
        let mut shell = Self::shell();
        let executable = shell
            .program
            .canonicalize()
            .unwrap_or_else(|_| shell.program.clone());
        match executable.file_name().and_then(OsStr::to_str) {
            Some("bash") => shell.args.insert(0, "--noediting".into()),
            Some("zsh") => shell.args = vec!["-o".into(), "nozle".into(), "-i".into()],
            _ => (),
        }
        shell
    }

    pub fn resolve(&mut self) -> Result<(), String> {
        if self.program.is_absolute() {
            return Ok(());
        }
        if self.program.components().count() != 1
            || self.program.file_name() != Some(self.program.as_os_str())
        {
            return Err("Use an absolute program path or a program on PATH.".into());
        }
        use std::os::unix::fs::PermissionsExt;
        self.program = std::env::split_paths(&child_path())
            .map(|dir| dir.join(&self.program))
            .find(|p| {
                p.metadata()
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            })
            .ok_or_else(|| format!("Cannot find {} on PATH.", self.program.display()))?;
        Ok(())
    }
    pub fn preview(&self) -> Result<String, String> {
        if !self.program.is_absolute() {
            return Err("The program path must be absolute.".into());
        }
        std::iter::once(self.program.as_os_str())
            .chain(self.args.iter().map(OsString::as_os_str))
            .map(quote)
            .collect::<Result<Vec<_>, _>>()
            .map(|parts| parts.join(" "))
    }
}

fn quote(value: &OsStr) -> Result<String, String> {
    let s = value
        .to_str()
        .ok_or("The command contains a non-UTF-8 argument.")?;
    if s.chars().any(|c| {
        (c.is_control() && !matches!(c, '\n' | '\t'))
            || matches!(c,'\u{061c}'|'\u{200e}'|'\u{200f}'|'\u{202a}'..='\u{202e}'|'\u{2066}'..='\u{2069}')
    }) {
        return Err("The command contains a hidden control character.".into());
    }
    if !s.is_empty()
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_./-+:,@%=".contains(&c))
    {
        Ok(s.to_owned())
    } else {
        Ok(format!("'{}'", s.replace('\'', "'\"'\"'")))
    }
}

/// Finder and desktop-entry launches often inherit a shorter PATH than terminals.
pub fn child_path() -> OsString {
    let mut paths: Vec<PathBuf> = env::var_os("PATH")
        .map(|path| {
            env::split_paths(&path)
                .filter(|p| p.is_absolute())
                .collect()
        })
        .unwrap_or_default();
    if let Some(home) = env::var_os("HOME") {
        let home = PathBuf::from(home);
        paths.extend(
            [
                ".local/bin",
                ".cargo/bin",
                ".local/share/mise/shims",
                ".local/share/mise/command-wrappers/bin",
                ".npm-global/bin",
                ".volta/bin",
            ]
            .map(|p| home.join(p)),
        );
    }
    paths.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"].map(PathBuf::from));
    env::join_paths(paths).unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into())
}

#[cfg(test)]
mod tests;
