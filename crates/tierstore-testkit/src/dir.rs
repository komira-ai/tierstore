//! [`TestDir`]: a scratch directory a test owns, removed when it is dropped.
//!
//! Tests used to make `<root>/<name>-<pid>` and never remove it, so every run left a
//! directory per test behind. A `TestDir` is unique per test (name, process id and a
//! counter), exists from [`TestDir::under`] on, and is removed on drop, except when the
//! thread is panicking and `KBF_KEEP_TMP` is set (a failed test's files are then left
//! for a person to read; the path is printed to stderr).
//!
//! Drop order matters when a test starts processes that write into the directory: a
//! `TestDir` drops after every local declared after it, so declare it first and the
//! processes (their guards reaped on drop) are gone before the directory is removed.

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// The variable that keeps the directory of a test that panicked.
pub const KEEP_ENV: &str = "KBF_KEEP_TMP";

/// A directory removed (with everything in it) on drop.
#[derive(Debug)]
pub struct TestDir {
    path: PathBuf,
}

impl TestDir {
    /// A new, empty directory `<root>/<name>-<pid>-<n>`, making `root` if needed.
    /// Integration tests pass `env!("CARGO_TARGET_TMPDIR")`; a unit test, which gets no
    /// such variable, passes [`std::env::temp_dir`].
    ///
    /// # Panics
    /// If the directory cannot be made (a test cannot go on without it).
    #[must_use]
    pub fn under(root: impl AsRef<Path>, name: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = root
            .as_ref()
            .join(format!("{name}-{}-{n}", std::process::id()));
        // Absent unless an earlier process of this pid left it.
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path)
            .unwrap_or_else(|e| panic!("make test dir {}: {e}", path.display()));
        Self { path }
    }

    /// The directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Deref for TestDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TestDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        if std::thread::panicking() && std::env::var_os(KEEP_ENV).is_some() {
            eprintln!("{KEEP_ENV} set: keeping {}", self.path.display());
            return;
        }
        match std::fs::remove_dir_all(&self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("could not remove test dir {}: {e}", self.path.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The root of the directories these tests make.
    fn root() -> PathBuf {
        std::env::temp_dir()
    }

    /// Catches a directory that is not made by `under`, or that the `Deref` and `AsRef` views
    /// do not name: all three are the one path, and it exists.
    #[test]
    fn a_new_dir_exists_and_every_view_names_it() {
        let dir = TestDir::under(root(), "testkit-views");
        assert!(dir.path().is_dir());
        assert_eq!(&*dir, dir.path());
        assert_eq!(dir.as_ref(), dir.path());
    }

    /// Catches a drop that leaves the directory, or one that fails on a directory that is
    /// already gone: the first is removed, the second drops quietly.
    #[test]
    fn a_dropped_dir_is_removed_and_a_missing_one_drops_quietly() {
        let dir = TestDir::under(root(), "testkit-drop");
        let path = dir.path().to_owned();
        drop(dir);
        assert!(!path.exists());

        let gone = TestDir::under(root(), "testkit-gone");
        std::fs::remove_dir_all(gone.path()).unwrap();
        drop(gone);
    }

    /// Catches a drop that panics when the path cannot be removed as a directory: a file put
    /// in its place makes the removal fail, and the drop only reports it.
    #[test]
    fn a_dir_that_cannot_be_removed_is_reported_not_panicked_on() {
        let dir = TestDir::under(root(), "testkit-file");
        let path = dir.path().to_owned();
        std::fs::remove_dir_all(&path).unwrap();
        std::fs::write(&path, b"not a directory").unwrap();
        drop(dir);
        assert!(path.is_file());
        std::fs::remove_file(&path).unwrap();
    }

    /// Catches an `under` that goes on without its directory: under a root that is a file the
    /// directory cannot be made, and `under` panics naming it.
    #[test]
    #[should_panic(expected = "make test dir")]
    fn a_dir_that_cannot_be_made_panics() {
        let file = TestDir::under(root(), "testkit-root");
        let not_a_dir = file.path().join("plain");
        std::fs::write(&not_a_dir, b"").unwrap();
        let _ = TestDir::under(&not_a_dir, "inside");
    }

    /// Catches a drop during a panic that keeps the directory without the variable, or
    /// removes it with the variable set: a thread that panics while it holds a directory
    /// leaves it only when `KEEP_ENV` is set.
    #[test]
    fn a_panic_keeps_the_dir_only_when_asked() {
        let in_panic = |name: &'static str| {
            let (sender, receiver) = std::sync::mpsc::channel();
            let joined = std::thread::spawn(move || {
                let dir = TestDir::under(root(), name);
                sender.send(dir.path().to_owned()).unwrap();
                panic!("the test panics while it holds {name}");
            })
            .join();
            assert!(joined.is_err());
            receiver.recv().unwrap()
        };
        let removed = in_panic("testkit-panic-removed");
        assert!(!removed.exists());

        // SAFETY: the tests run under nextest (`ci.yml`), each in a process of its own, so no
        // other thread reads or writes the environment while this test changes it.
        unsafe { std::env::set_var(KEEP_ENV, "1") };
        let kept = in_panic("testkit-panic-kept");
        // SAFETY: as above.
        unsafe { std::env::remove_var(KEEP_ENV) };
        assert!(kept.is_dir());
        std::fs::remove_dir_all(&kept).unwrap();
    }
}
