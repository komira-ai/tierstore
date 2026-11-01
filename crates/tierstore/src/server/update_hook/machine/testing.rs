//! A runner that records the calls and answers from a list, and a scratch directory.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;

use tierstore_testkit::dir::TestDir;

use super::{Call, Output, RunError, Runner};

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
