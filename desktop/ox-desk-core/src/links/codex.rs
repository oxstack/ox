//! One owned stdio process group. Only its writer thread touches stdin.
use crate::codex::{
    frame::{Frame, RequestId},
    names::Method,
    request::{Call, Outgoing},
};
use std::{
    collections::{HashMap, VecDeque},
    ffi::OsString,
    io::{self, BufRead, BufReader, BufWriter, Write},
    os::unix::process::CommandExt,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::Duration,
};

pub struct Launch {
    pub executable: PathBuf,
    pub arguments: Vec<OsString>,
    pub directory: PathBuf,
    pub path: OsString,
}

pub use super::LinkEvent;

pub struct Link {
    group: u32,
    methods: Arc<Mutex<HashMap<i64, Method>>>,
    writes: mpsc::Sender<Outgoing>,
    close: mpsc::Sender<String>,
}

impl Link {
    pub const DISPLAY_NAME: &'static str = "Codex";
    pub fn spawn(
        launch: Launch,
        receive: impl Fn(LinkEvent) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let mut command = Command::new(launch.executable);
        command
            .args(launch.arguments)
            .current_dir(launch.directory)
            .env("PATH", launch.path);
        Self::from_command(command, receive)
    }

    pub fn from_command(
        mut command: Command,
        receive: impl Fn(LinkEvent) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()?;
        let group = child.id();
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let (writes, messages) = mpsc::channel::<Outgoing>();
        let (close, closing) = mpsc::channel::<String>();
        let receive = Arc::new(receive);
        let output_event = receive.clone();
        let read_close = close.clone();
        let methods = Arc::new(Mutex::new(HashMap::new()));
        let reading = methods.clone();
        thread::spawn(move || {
            let result = (|| -> io::Result<()> {
                let mut output = BufReader::new(stdout);
                let mut bytes = Vec::new();
                loop {
                    bytes.clear();
                    if output.read_until(b'\n', &mut bytes)? == 0 {
                        return Ok(());
                    }
                    output_event(LinkEvent::Frame(Frame::decode(&bytes, |id| match id {
                        RequestId::Number(n) => reading.lock().unwrap().remove(n),
                        RequestId::Text(_) => None,
                    })?));
                }
            })();
            let _ = read_close.send(
                result
                    .err()
                    .map(|err| err.to_string())
                    .unwrap_or_else(|| "Codex closed its output".into()),
            );
        });
        let write_close = close.clone();
        thread::spawn(move || {
            let mut input = BufWriter::new(stdin);
            for message in messages {
                let result = serde_json::to_writer(&mut input, &message.encode())
                    .map_err(io::Error::from)
                    .and_then(|_| input.write_all(b"\n"))
                    .and_then(|_| input.flush());
                if let Err(err) = result {
                    let _ = write_close.send(err.to_string());
                    break;
                }
            }
        });
        let diagnostics = thread::spawn(move || {
            // Enough for a whole `ox` launch error (first line, why and fix).
            const LINES: usize = 12;
            let mut tail = VecDeque::with_capacity(LINES);
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if tail.len() == LINES {
                    tail.pop_front();
                }
                tail.push_back(line.chars().take(512).collect());
            }
            tail.into_iter().collect()
        });
        thread::spawn(move || {
            let reason = closing
                .recv()
                .unwrap_or_else(|_| "Connection closed".into());
            let group = -(child.id() as i32);
            // The direct child stays unreaped until the group signals are sent. Its PID
            // cannot be recycled as a new process group during the grace period.
            unsafe { libc::kill(group, libc::SIGTERM) };
            thread::sleep(Duration::from_millis(250));
            unsafe { libc::kill(group, libc::SIGKILL) };
            let _ = child.wait();
            let stderr_tail = diagnostics.join().unwrap_or_default();
            receive(LinkEvent::Closed {
                reason,
                stderr_tail,
            });
        });
        Ok(Self {
            group,
            writes,
            close,
            methods,
        })
    }

    pub(crate) fn process_group(&self) -> u32 {
        self.group
    }

    pub fn request(&self, seq: i64, call: Call) -> io::Result<()> {
        self.methods.lock().unwrap().insert(seq, call.method());
        let result = self.send(Outgoing::Request(seq, call));
        if result.is_err() {
            self.forget(seq);
        }
        result
    }
    pub(crate) fn forget(&self, seq: i64) {
        self.methods.lock().unwrap().remove(&seq);
    }
    pub fn initialized(&self) -> io::Result<()> {
        self.send(Outgoing::Initialized)
    }
    pub fn answer(&self, id: RequestId, answer: crate::codex::ask::Answer) -> io::Result<()> {
        self.send(Outgoing::Answer(id, answer))
    }
    pub fn reject(&self, id: RequestId, code: i64, message: &str) -> io::Result<()> {
        self.send(Outgoing::Reject(id, code, message.into()))
    }
    fn send(&self, message: Outgoing) -> io::Result<()> {
        self.writes
            .send(message)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "Codex writer closed"))
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        let _ = self.close.send("Connection closed by desktop".into());
    }
}
