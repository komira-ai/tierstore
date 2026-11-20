//! [`Tree`]: an image and its packs; the reader protocol of 5.1, parked waits and the lost path.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tierstore_fetch_api::engine::WaitOutcome;
use tierstore_fetch_api::ids::{ByteRange, JobId, TreeRef};
use tierstore_fetch_api::list::NodeIndex;
use tierstore_fetch_api::page::CELL_SHIFT;
use tierstore_mount_client::{Attempt, OpenSpec, Pack, PackClient};
use tokio::sync::watch;

use crate::error::TreeError;
use crate::image::{Image, NodeKind};
use crate::read::{ReadFailure, ReadStep, Stage, Staged, Wait, Waited};

/// What a tree is opened as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeConfig {
    /// The tree's reference, which `open-pack` names.
    pub tree: TreeRef,
    /// The job the mount serves: the waits it sends are this job's demand.
    pub job: JobId,
}

/// The counts of a tree since it was opened.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TreeStats {
    /// Reads served (committed), empty reads not counted.
    pub served: u64,
    /// Reads that found a covered cell not `present` and parked.
    pub parked: u64,
    /// Reads whose post-read recheck found a covered cell changed: their bytes were discarded and
    /// the read parked (also counted in `parked`).
    pub recheck_discarded: u64,
    /// Waits sent on the mount protocol.
    pub waits: u64,
    /// Waits not answered yet.
    pub waiting: u64,
    /// First reads of a cell in this tree's session that found it `present`
    /// (`first_touch_total{ready="yes"}`, 6.1).
    pub first_touch_ready: u64,
    /// First reads of a cell that found it not `present` (`first_touch_total{ready="no"}`).
    pub first_touch_unready: u64,
    /// Whether the tree is lost.
    pub lost: bool,
}

#[derive(Debug, Default)]
struct Counts {
    served: AtomicU64,
    parked: AtomicU64,
    recheck_discarded: AtomicU64,
    waits: AtomicU64,
    waiting: AtomicU64,
    first_touch_ready: AtomicU64,
    first_touch_unready: AtomicU64,
}

/// A pack of the tree, opened, and which of its cells this session has read.
#[derive(Debug)]
struct Slot {
    pack: Arc<Pack>,
    touched: Box<[AtomicU64]>,
}

/// Lowers [`Counts::waiting`] when a wait ends, answered or dropped.
struct Waiting<'a>(&'a AtomicU64);

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A tree mounted for one job: its image, its packs open through the mount protocol (a hold on
/// each for the mount's session, 7), and whether it is lost.
///
/// Dropping it is the mount's release of the tree; the holds go with the mount client's
/// [`release`](PackClient::release) or its process (the lifeline).
#[derive(Debug)]
pub struct Tree {
    image: Image,
    slots: Box<[Slot]>,
    job: JobId,
    lost: AtomicBool,
    lost_tx: watch::Sender<bool>,
    counts: Counts,
}

impl Tree {
    /// Opens `image` for `config.job`: sends `open-pack` for every pack of the tree at once and
    /// waits for them all.
    ///
    /// # Errors
    ///
    /// [`TreeError::Open`] with the first pack (by number) the mount client could not open.
    pub async fn open(
        image: Image,
        client: &PackClient,
        config: TreeConfig,
    ) -> Result<Self, TreeError> {
        let tickets = image.packs().iter().enumerate().map(|(index, pack)| {
            client.open(OpenSpec {
                tree: config.tree.clone(),
                index: index as u32,
                pack: pack.id.clone(),
                size: pack.size,
            })
        });
        let opened = futures::future::join_all(tickets).await;
        let mut slots = Vec::with_capacity(opened.len());
        for (index, result) in opened.into_iter().enumerate() {
            let pack = result.map_err(|source| TreeError::Open {
                pack: index as u32,
                source,
            })?;
            let words = u64::from(pack.layout().cell_count).div_ceil(64);
            slots.push(Slot {
                pack,
                touched: (0..words).map(|_| AtomicU64::new(0)).collect(),
            });
        }
        Ok(Self {
            image,
            slots: slots.into_boxed_slice(),
            job: config.job,
            lost: AtomicBool::new(false),
            lost_tx: watch::Sender::new(false),
            counts: Counts::default(),
        })
    }

    /// The tree's image: its entries, names and link targets.
    #[must_use]
    pub const fn image(&self) -> &Image {
        &self.image
    }

    /// Whether the tree is lost (4.6). A lost tree stays lost until it is dropped.
    #[must_use]
    pub fn is_lost(&self) -> bool {
        self.lost.load(Ordering::SeqCst)
    }

    /// Resolves once the tree is lost, at once if it already is.
    pub async fn lost(&self) {
        let mut rx = self.lost_tx.subscribe();
        // The sender lives as long as `self`, so the wait ends only when the value is true.
        let _ = rx.wait_for(|lost| *lost).await;
    }

    fn mark_lost(&self) {
        self.lost.store(true, Ordering::SeqCst);
        self.lost_tx.send_replace(true);
    }

    /// The counts since the tree was opened.
    #[must_use]
    pub fn stats(&self) -> TreeStats {
        let c = &self.counts;
        TreeStats {
            served: c.served.load(Ordering::Relaxed),
            parked: c.parked.load(Ordering::Relaxed),
            recheck_discarded: c.recheck_discarded.load(Ordering::Relaxed),
            waits: c.waits.load(Ordering::Relaxed),
            waiting: c.waiting.load(Ordering::Relaxed),
            first_touch_ready: c.first_touch_ready.load(Ordering::Relaxed),
            first_touch_unready: c.first_touch_unready.load(Ordering::Relaxed),
            lost: self.is_lost(),
        }
    }

    /// Counts the first read of each cell of `range` in this session, as `present` or not.
    fn touch(&self, slot: &Slot, range: ByteRange, ready: bool) {
        let Some(cells) = range.cells(CELL_SHIFT) else {
            return;
        };
        let count = if ready {
            &self.counts.first_touch_ready
        } else {
            &self.counts.first_touch_unready
        };
        for cell in cells.cells() {
            let bit = 1u64 << (cell % 64);
            let first = slot
                .touched
                .get((cell / 64) as usize)
                .is_some_and(|word| word.fetch_or(bit, Ordering::Relaxed) & bit == 0);
            count.fetch_add(u64::from(first), Ordering::Relaxed);
        }
    }

    /// Reads `size` bytes at `offset` of file `file` into `stage`, by the reader protocol of 5.1:
    ///
    /// 1. raise the reader counter of every 16-cell group the range covers (`SeqCst`);
    /// 2. load every covered cell's state (`SeqCst`): a `bad` cell fails the read, any other
    ///    state but `present` lowers the counters and parks it;
    /// 3. the stage reads the bytes from the pack's data descriptor;
    /// 4. the post-read recheck loads every covered cell again: if one is no longer `present`,
    ///    the bytes are discarded unserved, the counters lowered, and the read parks;
    /// 5. the bytes are committed, then the counters lowered.
    ///
    /// A read at or past the end of the file is [`ReadStep::Empty`]; a read past it is cut to it.
    /// Runs inline with no wait and no lock: on a FUSE reader thread, the stage's `pread` or
    /// `splice` of `present` cells is the only I/O.
    pub fn read<S: Stage>(&self, file: NodeIndex, offset: u64, size: u32, stage: S) -> ReadStep<S> {
        if self.is_lost() {
            return ReadStep::Failed(stage, ReadFailure::Lost);
        }
        let Some(node) = self.image.node(file) else {
            return ReadStep::Failed(stage, ReadFailure::NotFound);
        };
        let NodeKind::File {
            pack,
            offset: at,
            size: file_size,
        } = node.kind
        else {
            return ReadStep::Failed(stage, ReadFailure::NotAFile);
        };
        let len = file_size.saturating_sub(offset).min(u64::from(size));
        // A file of size 0 has no pack; any other has one the open checked.
        let (Some(pack), true) = (pack, len > 0) else {
            return ReadStep::Empty(stage);
        };
        let Some(slot) = self.slots.get(pack as usize) else {
            return ReadStep::Failed(stage, ReadFailure::NotFound);
        };
        // The open checked `at + file_size` against the pack; a record changed since may not fit.
        // `offset` is below `file_size` (`len > 0`), so `at + offset` fits when that does.
        let Some(span_end) = at.checked_add(file_size) else {
            return ReadStep::Failed(stage, ReadFailure::NotFound);
        };
        let range = ByteRange {
            offset: at + offset,
            len,
        };
        let wait = Wait {
            pack,
            range,
            span_end,
        };
        if let Some(error) = slot.pack.broken() {
            return ReadStep::Failed(stage, ReadFailure::Broken(error.clone()));
        }
        let guard = match slot.pack.begin_read(range) {
            Attempt::Ready(guard) => guard,
            Attempt::NotReady { bad } => {
                self.touch(slot, range, false);
                return match bad {
                    Some(cell) => ReadStep::Failed(stage, ReadFailure::Bad { cell }),
                    None => {
                        self.counts.parked.fetch_add(1, Ordering::Relaxed);
                        ReadStep::Park(stage, wait)
                    }
                };
            }
        };
        self.touch(slot, range, true);
        // `len` is at most `size`, a u32.
        let staged = match stage.fill(guard.view().data(), range.offset, len as usize) {
            Ok(staged) => staged,
            Err(error) => return ReadStep::FillFailed(error),
        };
        if guard.recheck() {
            let committed = staged.commit();
            drop(guard);
            self.counts.served.fetch_add(1, Ordering::Relaxed);
            ReadStep::Served(committed)
        } else {
            let stage = staged.discard();
            drop(guard);
            self.counts
                .recheck_discarded
                .fetch_add(1, Ordering::Relaxed);
            self.counts.parked.fetch_add(1, Ordering::Relaxed);
            ReadStep::Park(stage, wait)
        }
    }

    /// Waits for what a parked read needs: sends `wait` for the read's range on the mount
    /// protocol (coalesced by the client with other waits of its 8-cell window, re-sent across
    /// reconnects) and waits for its answer or for the tree to be lost, whichever is first.
    ///
    /// A `lost` answer marks the tree lost, which ends every other parked wait of the tree with
    /// [`Waited::Lost`] too, whatever pack it waits on. The future may be dropped (an
    /// INTERRUPT): the server's wait is left to finish on its own.
    pub async fn wait(&self, wait: &Wait) -> Waited {
        if self.is_lost() {
            return Waited::Lost;
        }
        let Some(slot) = self.slots.get(wait.pack as usize) else {
            return Waited::Lost;
        };
        self.counts.waits.fetch_add(1, Ordering::Relaxed);
        self.counts.waiting.fetch_add(1, Ordering::Relaxed);
        let _waiting = Waiting(&self.counts.waiting);
        let ticket = slot.pack.wait(wait.range, wait.span_end, self.job);
        let answer = tokio::select! {
            answer = ticket => answer,
            () = self.lost() => return Waited::Lost,
        };
        match answer {
            Ok(WaitOutcome::Ready) => Waited::Retry,
            Ok(WaitOutcome::Eio { cell }) => Waited::Eio { cell },
            Ok(WaitOutcome::Lost) => {
                self.mark_lost();
                Waited::Lost
            }
            Err(error) => Waited::Broken(error),
        }
    }
}
