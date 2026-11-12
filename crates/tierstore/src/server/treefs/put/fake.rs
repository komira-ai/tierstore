//! A stand-in for the node's tierstore behind the fill socket's `put` request, for the tests of the
//! put directory: it reads the staged file the request names, checks its size and SHA-256, and keeps
//! the bytes under the key, create-only, as the upstream does; a test can script the next answers,
//! and hold the answers until it lets them go.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use sha2::{Digest as _, Sha256};

/// What the fake knows and does.
#[derive(Default)]
pub(in crate::server) struct FakePuts {
    /// The objects put, by key.
    pub objects: Mutex<BTreeMap<String, Vec<u8>>>,
    /// The answers to give, in order, before any put is looked at.
    pub script: Mutex<VecDeque<String>>,
    /// The `put` lines received, in order.
    pub asked: Mutex<Vec<String>>,
    /// Whether the answers wait (see [`FakePuts::hold`]), and the puts that wait.
    held: Mutex<(bool, usize)>,
    changed: Condvar,
}

impl FakePuts {
    /// Makes every put wait, once received, until [`FakePuts::let_go`].
    pub(in crate::server) fn hold(&self) {
        self.held.lock().unwrap().0 = true;
    }

    /// Lets the waiting puts, and the next ones, be answered.
    pub(in crate::server) fn let_go(&self) {
        self.held.lock().unwrap().0 = false;
        self.changed.notify_all();
    }

    /// Waits until `count` puts are waiting.
    pub(in crate::server) fn wait_for_held(&self, count: usize) {
        let mut held = self.held.lock().unwrap();
        while held.1 < count {
            held = self.changed.wait(held).unwrap();
        }
    }

    /// The bytes of the object `key`.
    pub(in crate::server) fn object(&self, key: &str) -> Option<Vec<u8>> {
        self.objects.lock().unwrap().get(key).cloned()
    }

    /// The puts received.
    pub(in crate::server) fn count(&self) -> usize {
        self.asked.lock().unwrap().len()
    }

    /// The answer to the line `line`, a `put`.
    pub(in crate::server) fn answer(&self, line: &str) -> String {
        self.asked.lock().unwrap().push(line.to_owned());
        {
            let mut held = self.held.lock().unwrap();
            held.1 += 1;
            self.changed.notify_all();
            while held.0 {
                held = self.changed.wait(held).unwrap();
            }
            held.1 -= 1;
        }
        if let Some(answer) = self.script.lock().unwrap().pop_front() {
            return answer;
        }
        let mut words = line.splitn(5, ' ');
        let (Some("put"), Some(key), Some(sha256), Some(size), Some(path)) = (
            words.next(),
            words.next(),
            words.next(),
            words.next(),
            words.next().and_then(|rest| rest.strip_prefix("from ")),
        ) else {
            return "refused malformed".to_owned();
        };
        let Ok(bytes) = std::fs::read(path) else {
            return failed("stage");
        };
        if size.parse::<usize>() != Ok(bytes.len()) || hex::encode(Sha256::digest(&bytes)) != sha256
        {
            return failed("checksum");
        }
        let mut objects = self.objects.lock().unwrap();
        match objects.get(key) {
            Some(kept) if *kept == bytes => r#"{"ok":true,"created":false}"#.to_owned(),
            Some(_) => failed("exists_different"),
            None => {
                let _ = objects.insert(key.to_owned(), bytes);
                r#"{"ok":true,"created":true}"#.to_owned()
            }
        }
    }
}

/// The answer that a put failed for the reason `word`.
pub(in crate::server) fn failed(word: &str) -> String {
    format!(r#"{{"ok":false,"error":"{word}"}}"#)
}

/// [`FakePuts`] served on a Unix socket, one thread per connection, until dropped.
pub(in crate::server) struct FakePutSocket {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FakePutSocket {
    /// Serves `puts` at `path`.
    pub(in crate::server) fn start(path: &Path, puts: Arc<FakePuts>) -> Self {
        let listener = UnixListener::bind(path).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                if stopping.load(Ordering::Acquire) {
                    return;
                }
                let puts = Arc::clone(&puts);
                let _ = std::thread::spawn(move || {
                    let mut reader = BufReader::new(&stream);
                    let mut writer = &stream;
                    let mut line = String::new();
                    while reader.read_line(&mut line).unwrap_or(0) > 0 {
                        let answer = puts.answer(line.trim_end_matches('\n'));
                        let _ = writer.write_all(format!("{answer}\n").as_bytes());
                        line.clear();
                    }
                });
            }
        });
        Self {
            path: path.to_owned(),
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for FakePutSocket {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = UnixStream::connect(&self.path);
        let _ = self.thread.take().map(std::thread::JoinHandle::join);
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::{FakePuts, failed};

    /// Asserts the fake's own refusals, so that a test that gets one knows the request was wrong:
    /// a line that is not a `put` is refused, a staged file that cannot be read is `stage`, and
    /// bytes that are not the size or SHA-256 named are `checksum`.
    #[test]
    fn the_fake_refuses_what_the_upstream_would() {
        let puts = FakePuts::default();
        assert_eq!(puts.answer("fill x"), "refused malformed");
        assert_eq!(puts.answer("put k"), "refused malformed");
        let dir = tierstore_testkit::dir::TestDir::under(std::env::temp_dir(), "put-fake");
        let missing = dir.path().join("missing");
        let sha = "0".repeat(64);
        assert_eq!(
            puts.answer(&format!("put k {sha} 1 from {}", missing.display())),
            failed("stage")
        );
        let file = dir.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(
            puts.answer(&format!("put k {sha} 1 from {}", file.display())),
            failed("checksum")
        );
        assert_eq!(
            puts.answer(&format!("put k {sha} 2 from {}", file.display())),
            failed("checksum")
        );
        assert_eq!(puts.count(), 5);
        assert_eq!(puts.object("k"), None);
    }
}
