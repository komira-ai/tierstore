//! The watcher of the put root on macOS: one FSEvents stream of the root with the events of each
//! file (`kFSEventStreamCreateFlagFileEvents`, no latency), which is recursive by itself. Every path
//! an event names is a [`Seen::Path`] (a rename names both the old and the new path, so the
//! `.sha256-<hex>.part` name between kbf-task's two renames is seen though it is gone by then); a
//! directory is a [`Seen::Dir`]; an event that says events were dropped, or that the root changed,
//! is [`Seen::Lost`]. The stream runs on a thread of its own, the crate's, whose events a second
//! thread hands to the ingest's task.

use std::io;
use std::path::{Path, PathBuf};

use fsevent::{Event, FsEvent, StreamFlags};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use super::Seen;
use super::names::{Place, place};

/// The flags of an event that says the events of some paths were not delivered.
fn lost() -> StreamFlags {
    StreamFlags::MUST_SCAN_SUBDIRS
        | StreamFlags::USER_DROPPED
        | StreamFlags::KERNEL_DROPPED
        | StreamFlags::ROOT_CHANGED
}

/// The watcher of one root.
pub(super) struct Watcher {
    stream: FsEvent,
    events: UnboundedReceiver<Event>,
    /// The root as the events name it: with its links resolved.
    root: PathBuf,
}

impl Watcher {
    /// Watches `root`.
    ///
    /// # Errors
    /// The root cannot be resolved, or the stream cannot be started.
    pub(super) fn new(root: &Path) -> io::Result<Self> {
        let root = std::fs::canonicalize(root)?;
        let shown = root.to_str().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "the root is not UTF-8")
        })?;
        let mut stream = FsEvent::new(vec![shown.to_owned()]);
        let (to_bridge, from_stream) = std::sync::mpsc::channel();
        stream
            .observe_async(to_bridge)
            .map_err(|why| io::Error::other(why.to_string()))?;
        let (to_task, events) = unbounded_channel();
        let _ = std::thread::Builder::new()
            .name("tierstore-put-ingest-events".to_owned())
            .spawn(move || {
                while let Ok(event) = from_stream.recv() {
                    if to_task.send(event).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            stream,
            events,
            root,
        })
    }

    /// Waits for events and adds what they say to `seen`.
    ///
    /// # Errors
    /// The stream has ended.
    pub(super) async fn next(&mut self, seen: &mut Vec<Seen>) -> io::Result<()> {
        let first = self
            .events
            .recv()
            .await
            .ok_or_else(|| io::Error::other("the FSEvents stream ended"))?;
        self.take(&first, seen);
        while let Ok(event) = self.events.try_recv() {
            self.take(&event, seen);
        }
        Ok(())
    }

    /// Takes one event.
    fn take(&self, event: &Event, seen: &mut Vec<Seen>) {
        if event.flag.intersects(lost()) {
            seen.push(Seen::Lost);
            return;
        }
        let Ok(rel) = Path::new(&event.path).strip_prefix(&self.root) else {
            return;
        };
        let rel = rel.to_owned();
        let dir = matches!(place(&rel), Place::Above(_) | Place::Job(_));
        if event.flag.contains(StreamFlags::IS_DIR) && dir {
            seen.push(Seen::Dir(rel));
        } else {
            seen.push(Seen::Path(rel));
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stream.shutdown_observe();
    }
}
