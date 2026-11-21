//! A runner that records the calls and answers from a list, and a scratch directory.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tierstore_testkit::dir::TestDir;

use super::{Call, Output, RunError, Runner, StateFiles};

/// The time limit the tests give calls they do not time.
pub const LIMIT: Duration = Duration::from_secs(7);

/// The prefix of a program that is a descriptor of the hook's own (`/proc/self/fd/<n>`).
pub const OWN_DESCRIPTOR: &str = "/proc/self/fd/";

/// `call` as it executes: a program that is a descriptor of this process is the file it was
/// opened from, read while the call runs (the descriptor is closed after it). Every other call is
/// itself.
pub fn as_executed(call: &Call) -> Call {
    let mut call = call.clone();
    if call.program.starts_with(OWN_DESCRIPTOR) {
        let file = std::fs::read_link(&call.program).expect("the descriptor is open");
        call.program = file.to_string_lossy().into_owned();
    }
    call
}

/// What `call` is handed as its configuration, read while it runs: the file a `--config` of a
/// descriptor of this process names (a file in memory the hook made), or else its standard input.
pub fn handed(call: &Call) -> Option<Vec<u8>> {
    let config = call
        .args
        .iter()
        .skip_while(|arg| *arg != "--config")
        .nth(1)
        .filter(|path| path.starts_with(OWN_DESCRIPTOR));
    match config {
        Some(path) => Some(std::fs::read(path).expect("the descriptor is open")),
        None => call.stdin.clone(),
    }
}

/// Answers the calls in order and keeps them, each as it executes ([`as_executed`]), and what
/// each was handed ([`handed`]).
pub struct FakeRunner {
    calls: RefCell<Vec<Call>>,
    handed: RefCell<Vec<Option<Vec<u8>>>>,
    replies: RefCell<VecDeque<Result<Output, RunError>>>,
}

impl FakeRunner {
    /// A runner that gives `replies`, one to each call.
    pub fn replying(replies: Vec<Result<Output, RunError>>) -> FakeRunner {
        FakeRunner {
            calls: RefCell::new(Vec::new()),
            handed: RefCell::new(Vec::new()),
            replies: RefCell::new(replies.into()),
        }
    }

    /// The calls so far.
    pub fn calls(&self) -> Vec<Call> {
        self.calls.borrow().clone()
    }

    /// What each call so far was handed as its configuration ([`handed`]).
    pub fn handed(&self) -> Vec<Option<Vec<u8>>> {
        self.handed.borrow().clone()
    }
}

impl Runner for FakeRunner {
    fn run(&self, call: &Call) -> Result<Output, RunError> {
        self.calls.borrow_mut().push(as_executed(call));
        self.handed.borrow_mut().push(handed(call));
        self.replies
            .borrow_mut()
            .pop_front()
            .expect("a reply for every call")
    }
}

/// A program that exited with `code`.
pub fn exit(code: i32, stdout: &str, stderr: &str) -> Result<Output, RunError> {
    Ok(Output {
        code: Some(code),
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
    })
}

/// A program that a signal ended.
pub fn signalled() -> Result<Output, RunError> {
    Ok(Output {
        code: None,
        stdout: Vec::new(),
        stderr: Vec::new(),
    })
}

/// A fresh scratch directory and its path.
pub fn scratch(name: &str) -> (TestDir, PathBuf) {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let path = dir.path().to_path_buf();
    (dir, path)
}

/// The user the tests run as.
pub fn me() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// A scratch root holding a state directory `state` (mode 0700, the tests' own user). Returns
/// the scratch directory and the state directory's path.
pub fn scratch_state(name: &str) -> (TestDir, PathBuf) {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let state = dir.path().join("state");
    std::fs::create_dir(&state).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
    (dir, state)
}

/// The state files of `state`, owned by the tests' user.
pub fn files(state: &Path) -> StateFiles {
    StateFiles::new(state.to_path_buf(), me())
}
