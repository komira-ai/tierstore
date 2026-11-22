//! The watcher of the put root on macOS: one FSEvents stream of the root with the events of each
//! file (`kFSEventStreamCreateFlagFileEvents`, no latency), which is recursive by itself. Every path
//! an event names is a [`Seen::Path`] (a rename names both the old and the new path, so the
//! `.sha256-<hex>.part` name between kbf-task's two renames is seen though it is gone by then); a
//! directory is a [`Seen::Dir`]; an event that says events were dropped, or that the root changed,
//! is [`Seen::Lost`].
//!
//! The stream and its run loop are not `Send`, so they never leave the thread that makes them: that
//! thread starts the stream (the crate runs it on a thread of its own), hands its events to the
//! ingest's task, and stops it when the watcher is dropped.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use fsevent::{Event, FsEvent, StreamFlags};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

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
    /// Dropped with the watcher, which ends the thread of the stream, and the stream with it.
    _stop: mpsc::Sender<()>,
    events: UnboundedReceiver<Event>,
    /// The root as the events name it: with its links resolved.
    root: PathBuf,
}

/// The thread of the stream of `root`: starts it, says whether it started on `started`, passes its
/// events to `events` until `stop` is dropped, and stops it.
fn stream(
    root: String,
    started: mpsc::Sender<io::Result<()>>,
    events: UnboundedSender<Event>,
    stop: mpsc::Receiver<()>,
) {
    let mut stream = FsEvent::new(vec![root]);
    let (to_bridge, from_stream) = mpsc::channel();
    if let Err(why) = stream.observe_async(to_bridge) {
        let _ = started.send(Err(io::Error::other(why.to_string())));
        return;
    }
    let _ = started.send(Ok(()));
    let bridge = std::thread::Builder::new()
        .name("tierstore-put-ingest-events".to_owned())
        .spawn(move || {
            while let Ok(event) = from_stream.recv() {
                if events.send(event).is_err() {
                    break;
                }
            }
        });
    // Returns when the watcher is dropped.
    let _ = stop.recv();
    stream.shutdown_observe();
    if let Ok(bridge) = bridge {
        let _ = bridge.join();
    }
}

impl Watcher {
    /// Watches `root`.
    ///
    /// # Errors
    /// The root cannot be resolved, or the stream cannot be started.
    pub(super) fn new(root: &Path) -> io::Result<Self> {
        let root = std::fs::canonicalize(root)?;
        let shown = root
            .to_str()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the root is not UTF-8"))?
            .to_owned();
        let (started, starting) = mpsc::channel();
        let (to_task, events) = unbounded_channel();
        let (stop, stopping) = mpsc::channel();
        let _ = std::thread::Builder::new()
            .name("tierstore-put-ingest-stream".to_owned())
            .spawn(move || stream(shown, started, to_task, stopping))?;
        starting
            .recv()
            .map_err(|_| io::Error::other("the FSEvents thread ended before its stream started"))??;
        Ok(Self {
            _stop: stop,
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
