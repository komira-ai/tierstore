//! The watcher of the put root on Linux: inotify, with a watch on the root and on every directory
//! down to the jobs' directories (`cas/b/<build>/<nonce>`), each added when its directory appears.
//! A name that appears in a watched directory (made, or renamed into it) is a [`Seen::Path`]; a
//! directory that appears is watched at once and is a [`Seen::Dir`], since what was made in it
//! before its watch was added was not seen; an overflow of the queue is [`Seen::Lost`].

use std::collections::BTreeMap;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};
use rustix::io::Errno;
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

use super::Seen;
use super::names::{Place, place};

/// The bytes of events read at a time.
const BUFFER: usize = 64 << 10;

/// The events of a watched directory that are read.
fn flags() -> WatchFlags {
    WatchFlags::CREATE
        | WatchFlags::MOVED_TO
        | WatchFlags::ONLYDIR
        | WatchFlags::DONT_FOLLOW
        | WatchFlags::EXCL_UNLINK
}

/// Whether the directory `rel` is watched: the root, `cas`, `cas/b`, a build's directory or a
/// job's directory.
fn watched(rel: &Path) -> bool {
    matches!(place(rel), Place::Above(_) | Place::Job(_))
}

/// The watcher of one root.
pub(super) struct Watcher {
    fd: AsyncFd<OwnedFd>,
    root: PathBuf,
    /// The directory, relative to the root, of each watch.
    dirs: BTreeMap<i32, PathBuf>,
    buf: Vec<MaybeUninit<u8>>,
}

impl Watcher {
    /// Watches `root` and the directories under it that are watched; the watches are in place
    /// when it returns.
    ///
    /// # Errors
    /// The root cannot be watched, or inotify cannot be made.
    #[expect(
        deprecated,
        reason = "tokio deprecates the safe registration because a value that does not own its \
                  descriptor could close it while registered; an `OwnedFd` owns it for as long as \
                  the `AsyncFd` holds it, which is the condition of the `unsafe` replacement, so \
                  no `unsafe` is added here"
    )]
    pub(super) fn new(root: &Path) -> io::Result<Self> {
        let fd = inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK)?;
        let mut watcher = Self {
            fd: AsyncFd::with_interest(fd, Interest::READABLE)?,
            root: root.to_owned(),
            dirs: BTreeMap::new(),
            buf: vec![MaybeUninit::uninit(); BUFFER],
        };
        let wd = inotify::add_watch(watcher.fd.get_ref(), root, flags())?;
        let _ = watcher.dirs.insert(wd, PathBuf::new());
        watcher.watch_below(Path::new(""));
        Ok(watcher)
    }

    /// Watches the directories under `rel` that are watched, and the ones under them; one that
    /// cannot be watched (gone already) is left.
    fn watch_below(&mut self, rel: &Path) {
        // A directory that cannot be read (gone already) lists nothing.
        let entries = std::fs::read_dir(self.root.join(rel)).into_iter().flatten();
        for entry in entries.flatten() {
            let child = rel.join(entry.file_name());
            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            if is_dir && watched(&child) && self.watch(&child) {
                self.watch_below(&child);
            }
        }
    }

    /// Adds the watch of the directory `rel`; whether it was added.
    fn watch(&mut self, rel: &Path) -> bool {
        match inotify::add_watch(self.fd.get_ref(), self.root.join(rel), flags()) {
            Ok(wd) => {
                let _ = self.dirs.insert(wd, rel.to_owned());
                true
            }
            Err(_) => false,
        }
    }

    /// Waits for events and adds what they say to `seen`.
    ///
    /// # Errors
    /// The queue cannot be read.
    pub(super) async fn next(&mut self, seen: &mut Vec<Seen>) -> io::Result<()> {
        loop {
            let (made, ended) = {
                let mut ready = self.fd.readable().await?;
                let mut reader = inotify::Reader::new(self.fd.get_ref(), &mut self.buf);
                let mut made = Vec::new();
                let ended = loop {
                    match reader.next() {
                        Ok(event) => {
                            let name = event.file_name().map(|name| name.to_bytes().to_vec());
                            made.push((event.wd(), event.events(), name));
                        }
                        Err(Errno::AGAIN) => {
                            ready.clear_ready();
                            break Ok(());
                        }
                        Err(why) => break Err(io::Error::from(why)),
                    }
                };
                (made, ended)
            };
            for (wd, flags, name) in made {
                self.take(wd, flags, name, seen);
            }
            ended?;
            if !seen.is_empty() {
                return Ok(());
            }
        }
    }

    /// Takes one event.
    fn take(&mut self, wd: i32, flags: ReadFlags, name: Option<Vec<u8>>, seen: &mut Vec<Seen>) {
        if flags.contains(ReadFlags::QUEUE_OVERFLOW) {
            seen.push(Seen::Lost);
            return;
        }
        if flags.contains(ReadFlags::IGNORED) {
            let _ = self.dirs.remove(&wd);
        }
        // An event of no name (the end of a watch, just above) or of a watch dropped is nothing
        // seen.
        let (Some(dir), Some(name)) = (self.dirs.get(&wd), name) else {
            return;
        };
        let Ok(name) = String::from_utf8(name) else {
            return;
        };
        let rel = dir.join(name);
        if flags.contains(ReadFlags::ISDIR) && watched(&rel) {
            if self.watch(&rel) {
                self.watch_below(&rel);
            }
            seen.push(Seen::Dir(rel));
        } else {
            seen.push(Seen::Path(rel));
        }
    }
}
