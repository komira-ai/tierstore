//! A runner that records the calls and answers from a list, and a scratch directory.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tierstore_testkit::dir::TestDir;

use super::{Account, Call, Output, RunError, Runner, StateFiles};

/// The time limit the tests give calls they do not time.
pub const LIMIT: Duration = Duration::from_secs(7);

/// Answers the calls in order and keeps them.
pub struct FakeRunner {
    calls: RefCell<Vec<Call>>,
    replies: RefCell<VecDeque<Result<Output, RunError>>>,
}

impl FakeRunner {
    /// A runner that gives `replies`, one to each call.
    pub fn replying(replies: Vec<Result<Output, RunError>>) -> FakeRunner {
        FakeRunner {
            calls: RefCell::new(Vec::new()),
            replies: RefCell::new(replies.into()),
        }
    }

    /// The calls so far.
    pub fn calls(&self) -> Vec<Call> {
        self.calls.borrow().clone()
    }
}

impl Runner for FakeRunner {
    fn run(&self, call: &Call) -> Result<Output, RunError> {
        self.calls.borrow_mut().push(call.clone());
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

/// The service account the tests list in the group file.
pub const SERVICE: &str = "svc";

/// The user the tests run as.
pub fn me() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// The group the tests run as.
pub fn my_group() -> u32 {
    rustix::process::getegid().as_raw()
}

/// A scratch root holding a state directory `state` (mode 0750, the tests' own user and group)
/// and a group file, `group`, that lists the service account in the tests' group. Returns the
/// scratch directory and the state directory's path.
pub fn scratch_state(name: &str) -> (TestDir, PathBuf) {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let state = dir.path().join("state");
    std::fs::create_dir(&state).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o750)).unwrap();
    std::fs::write(
        dir.path().join("group"),
        format!("other:x:1:a\ntests:x:{}:b,{SERVICE}\n", my_group()),
    )
    .unwrap();
    (dir, state)
}

/// The account the tests give the state directory `state` of [`scratch_state`].
pub fn account(state: &Path) -> Account {
    Account {
        owner: me(),
        group: my_group(),
        service: SERVICE.to_owned(),
        group_file: state.parent().unwrap().join("group"),
    }
}

/// The state files of `state`, as [`account`] says.
pub fn files(state: &Path) -> StateFiles {
    StateFiles::new(state.to_path_buf(), account(state))
}
