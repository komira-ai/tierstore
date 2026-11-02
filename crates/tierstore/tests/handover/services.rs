//! The service manager of a run: the test's stand-in for systemd, which owns the server processes.
//! The hook's `systemctl` is the double (double/main.rs), which sends `start -- <unit>` or `stop --
//! <unit>` over the Unix socket `manager.sock` in the tools directory; the manager answers with an
//! exit code and what systemctl would say. The semantics are those of the slot unit template
//! (docs/design/tierstore-handover.md section 2) under `Type=simple`:
//!
//! - `start` of a slot unit whose `ConditionPathExists=` (the slot's binary, through its link)
//!   fails starts nothing and exits 0; of a running unit it does nothing; else it starts the
//!   binary with the run's arguments for that port and returns once the process is started.
//! - `stop` sends SIGTERM and returns once the process has exited (the drain runs in between);
//!   of a stopped unit it does nothing.
//! - Any other unit is not found (exit 5); any other verb is refused.
//!
//! Every call is logged, and every exit is kept with its status and its stderr, which the checks
//! read. A process's first line on stdout (its start line, printed once it listens) is awaited
//! with [`Services::wait_listening`]: an event, not a timer.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};

use rustix::process::{Pid, Signal, kill_process};
use tierstore::server::update_hook::unit_name;
use tierstore_testkit::reaped::{Reaped, spawn};

/// The arguments a slot's process is started with, by port: the unit's `ExecStart=` after the
/// binary.
pub type Exec = Box<dyn Fn(u16) -> Vec<String> + Send + Sync>;

/// How a process the manager started ended.
#[derive(Clone, Debug)]
pub struct Exit {
    pub port: u16,
    /// The exit status; `None` when a signal ended it.
    pub code: Option<i32>,
    /// What it wrote on stderr.
    pub stderr: String,
}

/// A process that runs, or ran, for a slot.
struct Process {
    child: Reaped,
    /// Its start line was printed.
    listening: bool,
    /// Its stdout ended: it exited, or is exiting.
    ended: bool,
    stderr: PathBuf,
}

#[derive(Default)]
struct State {
    /// The process of each running slot.
    running: BTreeMap<u16, Process>,
    /// One line per call: `start <port>`, `stop <port>`, `skip <port>` (the condition failed).
    calls: Vec<String>,
    exits: Vec<Exit>,
    /// The processes started so far, for the names of their logs.
    started: usize,
}

struct Inner {
    state: Mutex<State>,
    changed: Condvar,
    /// The state directory: a slot's binary is `<state>/slots/<port>/tierstore`.
    slots: PathBuf,
    ports: [u16; 2],
    exec: Exec,
    logs: PathBuf,
    closing: AtomicBool,
}

/// The manager and the thread that answers the double.
pub struct Services {
    inner: Arc<Inner>,
    socket: PathBuf,
    answering: Option<JoinHandle<()>>,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn port_of(&self, unit: &str) -> Option<u16> {
        self.ports.into_iter().find(|&port| unit == unit_name(port))
    }

    /// One call of systemctl: the exit code and what it says on stderr.
    fn call(self: &Arc<Self>, words: &[&str]) -> (u8, String) {
        let (verb, unit) = match words {
            [verb @ ("start" | "stop"), "--", unit] => (*verb, *unit),
            _ => return (1, format!("Unknown command verb {words:?}.\n")),
        };
        let Some(port) = self.port_of(unit) else {
            return (
                5,
                format!("Failed to {verb} {unit}: Unit {unit} not found.\n"),
            );
        };
        if verb == "start" {
            self.start(port)
        } else {
            self.stop(port);
            (0, String::new())
        }
    }

    fn start(self: &Arc<Self>, port: u16) -> (u8, String) {
        let mut state = self.lock();
        if state.running.contains_key(&port) {
            return (0, String::new());
        }
        let binary = self.slots.join(port.to_string()).join("tierstore");
        if !binary.exists() {
            state.calls.push(format!("skip {port}"));
            return (0, String::new());
        }
        state.calls.push(format!("start {port}"));
        state.started += 1;
        let name = format!("{port}-{}", state.started);
        let stderr = self.logs.join(format!("{name}.stderr"));
        let mut command = Command::new(&binary);
        command
            .args((self.exec)(port))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(fs::File::create(&stderr).expect("a log"));
        let mut child = match spawn(&mut command, 3) {
            Ok(child) => child,
            Err(error) => {
                return (1, format!("Job for {} failed: {error}\n", unit_name(port)));
            }
        };
        let stdout = child.stdout.take().expect("a pipe");
        let pid = child.id();
        state.running.insert(
            port,
            Process {
                child,
                listening: false,
                ended: false,
                stderr,
            },
        );
        drop(state);
        let inner = Arc::clone(self);
        let log = self.logs.join(format!("{name}.stdout"));
        thread::spawn(move || inner.watch_stdout(port, pid, stdout, &log));
        (0, String::new())
    }

    /// Copies a process's stdout to its log; its first line says it listens, its end that it
    /// exits.
    fn watch_stdout(&self, port: u16, pid: u32, stdout: impl Read, log: &Path) {
        let mut log = fs::File::create(log).expect("a log");
        let mut lines = BufReader::new(stdout).lines();
        let first = lines.next().and_then(Result::ok);
        if let Some(line) = &first {
            let _ = writeln!(log, "{line}");
        }
        self.mark(port, pid, |process| process.listening = first.is_some());
        for line in lines.map_while(Result::ok) {
            let _ = writeln!(log, "{line}");
        }
        self.mark(port, pid, |process| process.ended = true);
    }

    fn mark(&self, port: u16, pid: u32, change: impl FnOnce(&mut Process)) {
        let mut state = self.lock();
        if let Some(process) = state.running.get_mut(&port)
            && process.child.id() == pid
        {
            change(process);
        }
        self.changed.notify_all();
    }

    fn stop(&self, port: u16) {
        let taken = {
            let mut state = self.lock();
            state.calls.push(format!("stop {port}"));
            state.running.remove(&port)
        };
        let Some(mut process) = taken else { return };
        let pid = Pid::from_raw(process.child.id().cast_signed()).expect("a pid");
        let _ = kill_process(pid, Signal::TERM);
        let status = process.child.wait().expect("wait for the process");
        let stderr = fs::read_to_string(&process.stderr).unwrap_or_default();
        let mut state = self.lock();
        state.exits.push(Exit {
            port,
            code: status.code(),
            stderr,
        });
        self.changed.notify_all();
    }

    /// Answers the double until the manager closes.
    fn answer(self: &Arc<Self>, listener: &UnixListener) {
        for stream in listener.incoming() {
            if self.closing.load(Ordering::SeqCst) {
                return;
            }
            let Ok(mut stream) = stream else { continue };
            let mut asked = String::new();
            if stream.read_to_string(&mut asked).is_err() {
                continue;
            }
            let words: Vec<&str> = asked.split('\n').collect();
            let (code, said) = self.call(&words);
            let _ = stream.write_all(format!("{code}\n{said}").as_bytes());
        }
    }
}

impl Services {
    /// A manager for the slots `ports`, whose binaries are under `<state>/slots`, started with
    /// the arguments `exec` gives, logging to `logs`, answering on `<tools>/manager.sock`.
    pub fn start(tools: &Path, state: &Path, ports: [u16; 2], exec: Exec, logs: &Path) -> Services {
        fs::create_dir_all(logs).expect("the logs");
        let socket = tools.join("manager.sock");
        let listener = UnixListener::bind(&socket).expect("the manager's socket");
        let inner = Arc::new(Inner {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            slots: state.join("slots"),
            ports,
            exec,
            logs: logs.to_owned(),
            closing: AtomicBool::new(false),
        });
        let answering = {
            let inner = Arc::clone(&inner);
            thread::spawn(move || inner.answer(&listener))
        };
        Services {
            inner,
            socket,
            answering: Some(answering),
        }
    }

    /// `systemctl stop` of the slot `port`.
    pub fn stop_slot(&self, port: u16) {
        self.inner.stop(port);
    }

    /// Waits until the process of `port` has printed its start line: it listens. Panics if it
    /// ended first, or if `port` is not running.
    pub fn wait_listening(&self, port: u16) {
        let mut state = self.inner.lock();
        loop {
            let process = state
                .running
                .get(&port)
                .unwrap_or_else(|| panic!("{port} is not running"));
            assert!(
                process.listening || !process.ended,
                "{port} ended before it listened: {}",
                fs::read_to_string(&process.stderr).unwrap_or_default()
            );
            if process.listening {
                return;
            }
            state = self
                .inner
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// The ports whose process runs.
    pub fn running(&self) -> Vec<u16> {
        self.inner.lock().running.keys().copied().collect()
    }

    /// The calls so far.
    pub fn calls(&self) -> Vec<String> {
        self.inner.lock().calls.clone()
    }

    /// The exits so far.
    pub fn exits(&self) -> Vec<Exit> {
        self.inner.lock().exits.clone()
    }
}

impl Drop for Services {
    /// Kills what still runs (a passing run has stopped it), and closes the socket.
    fn drop(&mut self) {
        self.inner.closing.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.socket);
        if let Some(answering) = self.answering.take() {
            let _ = answering.join();
        }
        let running = std::mem::take(&mut self.inner.lock().running);
        for (_, mut process) in running {
            let _ = process.child.kill();
            let _ = process.child.wait();
        }
    }
}
