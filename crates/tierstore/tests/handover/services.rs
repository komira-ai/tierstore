//! The service manager of a run: the test's stand-in for systemd, which owns the server processes.
//! The hook's `systemctl` is the double (double/main.rs), which sends `start -- <unit>` or `stop --
//! <unit>` over the Unix socket [`SOCKET`] in the tools directory; the manager answers with an
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

/// The Unix socket of the manager in the tools directory, which the double finds beside it. Short:
/// a socket's whole path must fit in 107 bytes.
pub const SOCKET: &str = "mgr";

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

impl Process {
    fn pid(&self) -> Pid {
        Pid::from_raw(self.child.id().cast_signed()).expect("a pid")
    }
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
        if let Some(process) = state.running.get(&port) {
            if !process.ended {
                return (0, String::new());
            }
            // It ended by itself: the unit is inactive, and this start starts it anew.
            let process = state.running.remove(&port).expect("the process");
            state.exits.push(Self::reap(port, process));
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

    /// Waits for a process that has ended (or is killed) and says how it ended.
    fn reap(port: u16, mut process: Process) -> Exit {
        let status = process.child.wait().expect("wait for the process");
        let stderr = fs::read_to_string(&process.stderr)
            .unwrap_or_else(|e| panic!("the stderr of {port}: {e}"));
        Exit {
            port,
            code: status.code(),
            stderr,
        }
    }

    /// SIGTERM, then waits for the end of the process's stdout (it exits), then reaps it. The
    /// process stays in the map meanwhile, so that a cleanup that comes while the drain runs can
    /// kill it.
    fn stop(&self, port: u16) {
        let mut state = self.lock();
        state.calls.push(format!("stop {port}"));
        let Some(process) = state.running.get(&port) else {
            return;
        };
        let _ = kill_process(process.pid(), Signal::TERM);
        while state.running.get(&port).is_some_and(|p| !p.ended) {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        let Some(process) = state.running.remove(&port) else {
            return;
        };
        drop(state);
        let exit = Self::reap(port, process);
        self.lock().exits.push(exit);
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
    /// the arguments `exec` gives, logging to `logs`, answering on `<tools>/`[`SOCKET`].
    pub fn start(tools: &Path, state: &Path, ports: [u16; 2], exec: Exec, logs: &Path) -> Services {
        fs::create_dir_all(logs).expect("the logs");
        let socket = tools.join(SOCKET);
        // A Unix socket's path has room for 107 bytes: a deep target directory (the coverage run
        // builds in target/llvm-cov-target) must not make the bind fail with a riddle.
        let length = socket.as_os_str().len();
        assert!(
            length < 108,
            "{} is {length} bytes, more than a Unix socket's path holds",
            socket.display()
        );
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

    /// The ports whose process runs: started, and not ended.
    pub fn running(&self) -> Vec<u16> {
        let state = self.inner.lock();
        let running = state.running.iter().filter(|(_, p)| !p.ended);
        running.map(|(port, _)| *port).collect()
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
    /// Kills what still runs (a passing run has stopped it), a process in the middle of a stop
    /// included, so that a stop the answering thread waits in ends; then closes the socket and
    /// reaps what is left.
    fn drop(&mut self) {
        self.inner.closing.store(true, Ordering::SeqCst);
        for process in self.inner.lock().running.values() {
            let _ = kill_process(process.pid(), Signal::KILL);
        }
        let _ = UnixStream::connect(&self.socket);
        if let Some(answering) = self.answering.take() {
            let _ = answering.join();
        }
        let running = std::mem::take(&mut self.inner.lock().running);
        for (_, mut process) in running {
            let _ = process.child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    //! The manager's unit semantics, with shell scripts for the slots' binaries: no privilege,
    //! so these run in the workspace's test run.

    use std::fs;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::path::Path;
    use std::thread;

    use tierstore::server::update_hook::unit_name;
    use tierstore_testkit::dir::TestDir;

    use std::sync::{Arc, Mutex};

    use super::{Exec, Services};

    /// Gives the slot `port` a binary: a link to `/bin/sh`, which the manager runs with `-c` and
    /// the run's script. No file is written and then executed, so no other test's fork can hold
    /// it open for writing (ETXTBSY).
    fn slot(state: &Path, port: u16) {
        let dir = state.join("slots").join(port.to_string());
        fs::create_dir_all(&dir).expect("a slot");
        let binary = dir.join("tierstore");
        if !binary.exists() {
            std::os::unix::fs::symlink("/bin/sh", &binary).expect("a link");
        }
    }

    /// A script that prints a start line and then waits for a signal.
    const LIVES: &str = "echo ready; exec tail -f /dev/null";
    /// The same, deaf to SIGTERM.
    const DEAF: &str = "trap '' TERM; echo ready; exec tail -f /dev/null";
    /// One that exits 1 before it prints anything.
    const DIES: &str = "exit 1";

    /// A manager whose slots run `sh -c` with the script `script` holds when they start.
    fn services(dir: &TestDir, script: &Arc<Mutex<&'static str>>) -> Services {
        let tools = dir.join("t");
        fs::create_dir_all(&tools).expect("tools");
        let script = Arc::clone(script);
        let exec: Exec = Box::new(move |_| {
            let script = *script.lock().expect("the script");
            vec!["-c".to_owned(), script.to_owned()]
        });
        Services::start(
            &tools,
            &dir.join("state"),
            [9301, 9302],
            exec,
            &dir.join("logs"),
        )
    }

    fn call(services: &Services, verb: &str, port: u16) -> u8 {
        services.inner.call(&[verb, "--", &unit_name(port)]).0
    }

    /// Asserts the unit semantics the hook relies on: a start whose binary (its link) is absent
    /// starts nothing and succeeds; a second start of a running unit starts nothing more; a stop
    /// waits for the exit and a second stop does nothing; an unknown unit is not found. It would
    /// catch a double that starts a slot with no link, or that runs a slot twice.
    #[test]
    fn a_start_without_a_link_is_skipped_and_starts_and_stops_are_idempotent() {
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "t1sv-a");
        let services = services(&dir, &Arc::new(Mutex::new(LIVES)));
        assert_eq!(call(&services, "start", 9302), 0);
        assert!(services.running().is_empty());
        slot(&dir.join("state"), 9301);
        assert_eq!(call(&services, "start", 9301), 0);
        assert_eq!(call(&services, "start", 9301), 0);
        services.wait_listening(9301);
        assert_eq!(services.running(), [9301]);
        assert_eq!(call(&services, "stop", 9301), 0);
        assert_eq!(call(&services, "stop", 9301), 0);
        assert!(services.running().is_empty());
        assert_eq!(services.exits().len(), 1);
        assert_eq!(
            services.calls(),
            ["skip 9302", "start 9301", "stop 9301", "stop 9301"]
        );
        let (code, said) = services.inner.call(&["start", "--", "other.service"]);
        assert_eq!(code, 5, "{said}");
    }

    /// Asserts that a process that exits before it prints its start line is reported by
    /// `wait_listening` (a panic, not a hang), counts as stopped, and that the next start
    /// launches a new process, as systemd starts an inactive unit anew.
    #[test]
    fn a_process_that_dies_before_it_listens_is_reported_and_started_anew() {
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "t1sv-b");
        let script = Arc::new(Mutex::new(DIES));
        let services = services(&dir, &script);
        slot(&dir.join("state"), 9301);
        assert_eq!(call(&services, "start", 9301), 0);
        let waited = catch_unwind(AssertUnwindSafe(|| services.wait_listening(9301)));
        assert!(
            waited.is_err(),
            "a process that died was waited for as listening"
        );
        assert!(services.running().is_empty());
        *script.lock().expect("the script") = LIVES;
        assert_eq!(call(&services, "start", 9301), 0);
        services.wait_listening(9301);
        assert_eq!(services.running(), [9301]);
        let exits = services.exits();
        assert_eq!((exits.len(), exits[0].code), (1, Some(1)));
    }

    /// Asserts that dropping the manager while a stop waits for a process that ignores SIGTERM
    /// (a hung drain) ends: the process is killed, the stop the double asked for returns, and the
    /// drop does not hang. It would catch a cleanup that cannot reach a process being stopped.
    #[test]
    fn dropping_the_manager_ends_a_stop_that_hangs() {
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "t1sv-c");
        let services = services(&dir, &Arc::new(Mutex::new(DEAF)));
        slot(&dir.join("state"), 9301);
        assert_eq!(call(&services, "start", 9301), 0);
        services.wait_listening(9301);
        let socket = dir.join("t").join(super::SOCKET);
        let asking = thread::spawn(move || {
            let mut stream = UnixStream::connect(socket).expect("the manager");
            stream
                .write_all(format!("stop\n--\n{}", unit_name(9301)).as_bytes())
                .expect("ask");
            stream.shutdown(std::net::Shutdown::Write).expect("ask");
            let mut answer = String::new();
            let _ = stream.read_to_string(&mut answer);
            answer
        });
        // The stop is under way once it is logged; it cannot end while the process ignores TERM.
        while !services.calls().contains(&"stop 9301".to_owned()) {
            thread::yield_now();
        }
        drop(services);
        let answer = asking.join().expect("the double's call");
        assert!(answer.is_empty() || answer.starts_with("0\n"), "{answer:?}");
    }
}
